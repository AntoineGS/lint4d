unit Lint4dFixture.Inheritance;

interface

uses
  Lint4dFixture.Classes;

type
  TBase = class
  private
    FId: Integer;
    FHelper: TSimpleClass;
  public
    constructor Create(AId: Integer); virtual;
    destructor Destroy; override;
    function GetId: Integer; virtual;
    procedure DoWork; virtual; abstract;
  end;

  TMiddle = class(TBase)
  private
    FLabel: string;
  public
    constructor Create(AId: Integer); override;
    function GetId: Integer; override;
    function GetLabel: string; virtual;
  end;

  TLeaf = class(TMiddle)
  private
    FActive: Boolean;
  public
    constructor Create(AId: Integer); override;
    procedure DoWork; override;
    function GetLabel: string; override;
  end;

  TAbstractBase = class abstract
  public
    procedure Process; virtual; abstract;
    function Validate: Boolean; virtual; abstract;
    class function ClassName_: string; virtual; abstract;
  end;

implementation

{ TBase }

constructor TBase.Create(AId: Integer);
begin
  inherited Create;
  FId := AId;
  FHelper := TSimpleClass.Create('Helper', AId);
end;

destructor TBase.Destroy;
begin
  FHelper.Free;
  inherited;
end;

function TBase.GetId: Integer;
begin
  Result := FId;
end;

{ TMiddle }

constructor TMiddle.Create(AId: Integer);
begin
  inherited;
  FLabel := 'Middle';
end;

function TMiddle.GetId: Integer;
begin
  Result := inherited GetId * 10;
end;

function TMiddle.GetLabel: string;
begin
  Result := FLabel;
end;

{ TLeaf }

constructor TLeaf.Create(AId: Integer);
begin
  inherited;
  FActive := True;
end;

procedure TLeaf.DoWork;
begin
  FActive := not FActive;
end;

function TLeaf.GetLabel: string;
begin
  Result := 'Leaf:' + inherited GetLabel;
end;

end.
