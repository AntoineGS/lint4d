unit Probe;

interface

type
  TFoo = class
  private
    FUnused: Integer;
    procedure UnusedPrivate;
  public
    procedure Renamed(A: Integer);
    procedure Missing;
  end;

procedure Standalone(X: Integer);

implementation

procedure TFoo.UnusedPrivate;
begin
end;

procedure TFoo.Renamed(B: Integer);
begin
end;

procedure Standalone(X: Integer);
begin
end;

end.
