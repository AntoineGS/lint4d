unit WarnAllOnAfterOff;

interface

procedure DeprecatedRoutine; deprecated;

{$WARN ALL OFF}

type
  TObject = class
  end;

  TBaseFieldOff = class(TObject)
  protected
    ValueOff: Integer;
  end;

  TChildFieldOff = class(TBaseFieldOff)
  private
    FChildValueOff: Integer;
  public
    property ValueOff: Integer read FChildValueOff;
  end;

  TBaseVirtualOff = class(TObject)
  public
    procedure InvokeOff; virtual;
  end;

  TChildVirtualOff = class(TBaseVirtualOff)
  public
    procedure InvokeOff;
  end;

  TUnusedField = class(TObject)
  private
    FUnused: Integer;
  end;

{$WARN ALL ON}

type
  TBaseFieldOn = class(TObject)
  protected
    ValueOn: Integer;
  end;

  TChildFieldOn = class(TBaseFieldOn)
  private
    FChildValueOn: Integer;
  public
    property ValueOn: Integer read FChildValueOn;
  end;

  TBaseVirtualOn = class(TObject)
  public
    procedure InvokeOn; virtual;
  end;

  TChildVirtualOn = class(TBaseVirtualOn)
  public
    procedure InvokeOn;
  end;

implementation

procedure DeprecatedRoutine;
begin
end;

procedure ExerciseWarnings;
begin
  {$WARN ALL OFF}
  DeprecatedRoutine;
  {$WARN ALL ON}
  DeprecatedRoutine;
end;

procedure TBaseVirtualOff.InvokeOff;
begin
end;

procedure TChildVirtualOff.InvokeOff;
begin
end;

procedure TBaseVirtualOn.InvokeOn;
begin
end;

procedure TChildVirtualOn.InvokeOn;
begin
end;

end.
