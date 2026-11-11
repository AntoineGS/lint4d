unit Clean;

interface

type
  TBase = class
  private
    FValue: Integer;
  public
    procedure Touch;
    procedure Execute; virtual;
  end;

  TChild = class(TBase)
  public
    procedure Execute; override;
  end;

implementation

procedure TBase.Touch;
begin
  Inc(FValue);
end;

procedure TBase.Execute;
begin
end;

procedure TChild.Execute;
begin
  inherited;
end;

end.
